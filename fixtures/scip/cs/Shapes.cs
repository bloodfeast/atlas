namespace Fixture.Shapes
{
    /// <summary>Something with an area.</summary>
    public interface IShape
    {
        double Area();
    }

    /// <summary>A square.</summary>
    public class Square : IShape
    {
        public Square(double side)
        {
            Side = side;
        }

        public double Side { get; }

        public double Area()
        {
            return Side * Side;
        }
    }

    public static class Sums
    {
        public static double Total(IShape[] shapes)
        {
            double sum = 0;
            foreach (var s in shapes)
            {
                sum += s.Area();
            }
            return sum;
        }
    }
}
